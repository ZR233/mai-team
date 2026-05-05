<template>
  <aside class="agent-rail task-rail" :class="{ hidden: !visible }">
    <div class="rail-head">
      <div>
        <h2>Tasks</h2>
        <p>{{ tasks.length }} task{{ tasks.length === 1 ? '' : 's' }}</p>
      </div>
      <button class="small-button" @click="$emit('create')">New Task</button>
    </div>
    <div class="agent-list">
      <button
        v-for="row in treeRows"
        :key="row.type + row.id"
        class="agent-row"
        :class="{
          active: row.type === 'task' ? selectedTaskId === row.id : selectedAgentId === row.id,
          nested: row.depth > 0,
          'task-row': row.type === 'task',
          'subagent-row': row.type === 'agent'
        }"
        :style="{ '--tree-indent': `${10 + row.depth * 20}px` }"
        @click="selectRow(row)"
      >
        <span class="tree-guide" aria-hidden="true"></span>
        <span class="avatar">{{ row.type === 'task' ? initial(row.task.title) : roleInitial(row.agent.role) }}</span>
        <span class="agent-row-main">
          <span class="agent-name">
            <span class="agent-name-text">{{ row.type === 'task' ? row.task.title : row.agent.name }}</span>
            <span v-if="row.type === 'task'" class="agent-child-count">{{ row.task.agent_count || 1 }}</span>
          </span>
          <span class="agent-meta">
            <template v-if="row.type === 'task'">
              {{ formatStatus(row.task.status) }} · Plan {{ formatStatus(row.task.plan_status) }}
            </template>
            <template v-else>
              {{ formatStatus(row.agent.role) }} · {{ row.agent.model }}
            </template>
          </span>
        </span>
        <span
          class="status-dot"
          :class="statusTone(row.type === 'task' ? row.task.status : row.agent.status)"
          :title="formatStatus(row.type === 'task' ? row.task.status : row.agent.status)"
        />
      </button>
      <div v-if="!tasks.length" class="empty-rail">
        <strong>No tasks yet</strong>
        <span>Create a task to start planning with a planner agent.</span>
      </div>
    </div>
  </aside>
</template>

<script setup>
import { computed } from 'vue'
import { formatStatus, initial, statusTone } from '../utils/format'
import { buildTaskTreeRows } from '../utils/taskTree'

const props = defineProps({
  tasks: { type: Array, required: true },
  detail: { type: Object, default: null },
  selectedTaskId: { type: String, default: null },
  selectedAgentId: { type: String, default: null },
  visible: { type: Boolean, default: true }
})

const emit = defineEmits(['select-task', 'select-agent', 'create'])

const treeRows = computed(() => buildTaskTreeRows(props.tasks, props.detail))

function selectRow(row) {
  if (row.type === 'task') emit('select-task', row.id)
  else emit('select-agent', row.id)
}

function roleInitial(role) {
  const value = String(role || '').toLowerCase()
  if (value === 'planner') return 'P'
  if (value === 'explorer') return 'X'
  if (value === 'executor') return 'E'
  if (value === 'reviewer') return 'R'
  return 'A'
}
</script>
