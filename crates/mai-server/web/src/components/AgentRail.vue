<template>
  <aside class="agent-rail" :class="{ hidden: !visible }">
    <div class="rail-head">
      <div>
        <h2>Task Agents</h2>
        <p>{{ agents.length }} agent{{ agents.length === 1 ? '' : 's' }}</p>
      </div>
      <button class="small-button" @click="$emit('create')">New Task Agent</button>
    </div>
    <div class="agent-list">
      <button
        v-for="row in treeRows"
        :key="row.agent.id"
        class="agent-row"
        :class="{ active: selectedId === row.agent.id, orphan: row.is_orphan, nested: row.depth > 0 }"
        :style="{ '--tree-indent': `${10 + row.depth * 20}px` }"
        @click="$emit('select', row.agent.id)"
      >
        <span class="tree-guide" aria-hidden="true"></span>
        <span class="avatar">{{ initial(row.agent.name) }}</span>
        <span class="agent-row-main">
          <span class="agent-name">
            <span class="agent-name-text">{{ row.agent.name }}</span>
            <span v-if="row.child_count" class="agent-child-count">{{ row.child_count }}</span>
          </span>
          <span class="agent-meta">
            {{ row.agent.provider_name }} / {{ row.agent.model }}
            <span v-if="row.is_orphan"> (detached)</span>
          </span>
        </span>
        <span class="status-dot" :class="statusTone(row.agent.status)" :title="formatStatus(row.agent.status)" />
      </button>
      <div v-if="!agents.length" class="empty-rail">
        <strong>No task agents yet</strong>
        <span>Create a task to start the planner.</span>
      </div>
    </div>
  </aside>
</template>

<script setup>
import { computed } from 'vue'
import { initial, statusTone, formatStatus } from '../utils/format'
import { buildAgentTreeRows } from '../utils/agentTree'

const props = defineProps({
  agents: { type: Array, required: true },
  selectedId: { type: String, default: null },
  visible: { type: Boolean, default: true }
})

defineEmits(['select', 'create'])

const treeRows = computed(() => buildAgentTreeRows(props.agents))
</script>
