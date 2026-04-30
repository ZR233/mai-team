<template>
  <aside class="agent-rail" :class="{ hidden: !visible }">
    <div class="rail-head">
      <div>
        <h2>Agents</h2>
        <p>{{ agents.length }} active session{{ agents.length === 1 ? '' : 's' }}</p>
      </div>
      <button class="small-button" @click="$emit('create')">New Agent</button>
    </div>
    <div class="agent-list">
      <button
        v-for="agent in agents"
        :key="agent.id"
        class="agent-row"
        :class="{ active: selectedId === agent.id }"
        @click="$emit('select', agent.id)"
      >
        <span class="avatar">{{ initial(agent.name) }}</span>
        <span class="agent-row-main">
          <span class="agent-name">{{ agent.name }}</span>
          <span class="agent-meta">{{ agent.provider_name }} / {{ agent.model }}</span>
        </span>
        <span class="status-dot" :class="statusTone(agent.status)" :title="formatStatus(agent.status)" />
      </button>
      <div v-if="!agents.length" class="empty-rail">
        <strong>No agents yet</strong>
        <span>Create one to start a container-backed session.</span>
      </div>
    </div>
  </aside>
</template>

<script setup>
import { initial, statusTone, formatStatus } from '../utils/format'

defineProps({
  agents: { type: Array, required: true },
  selectedId: { type: String, default: null },
  visible: { type: Boolean, default: true }
})

defineEmits(['select', 'create'])
</script>
